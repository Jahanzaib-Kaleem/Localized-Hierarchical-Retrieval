use memmap2::{Mmap, MmapMut};
use std::{fs::{File, OpenOptions}, io::{self, BufReader, Read}, path::Path};

const MAGIC: &[u8;8] = b"LHRPOST1";
const HEADER: usize = 40;
const DIR: usize = 24; // key u64, start u64, len u32, reserved u32

pub struct PostingHierarchy {
    map: Mmap,
    keys: usize,
    rows: usize,
    body: usize,
}

fn read_sparse<R: Read>(r: &mut R) -> io::Result<Option<(u64,u32)>> {
    let mut b=[0u8;12]; let mut got=0;
    while got<12 { match r.read(&mut b[got..])? { 0 if got==0=>return Ok(None),0=>return Err(io::Error::new(io::ErrorKind::UnexpectedEof,"truncated postings source")),n=>got+=n } }
    Ok(Some((u64::from_le_bytes(b[..8].try_into().unwrap()),u32::from_le_bytes(b[8..].try_into().unwrap()))))
}

impl PostingHierarchy {
    pub fn build_from_sorted(sorted: impl AsRef<Path>, output: impl AsRef<Path>, rows: u64) -> io::Result<()> {
        let mut reader=BufReader::new(File::open(sorted.as_ref())?); let mut unique=0u64; let mut records=0u64; let mut last=None;
        while let Some((k,_))=read_sparse(&mut reader)? { if last!=Some(k){unique+=1;last=Some(k)} records+=1; }
        if records!=rows { return Err(io::Error::new(io::ErrorKind::InvalidData,format!("postings require exactly one row reference per source row: {records} != {rows}"))); }
        let dir_bytes=unique.checked_mul(DIR as u64).ok_or_else(||io::Error::new(io::ErrorKind::InvalidInput,"postings directory overflow"))?;
        let body_bytes=records.checked_mul(4).ok_or_else(||io::Error::new(io::ErrorKind::InvalidInput,"postings body overflow"))?;
        let total=(HEADER as u64).checked_add(dir_bytes).and_then(|x|x.checked_add(body_bytes)).ok_or_else(||io::Error::new(io::ErrorKind::InvalidInput,"postings size overflow"))?;
        let f=OpenOptions::new().read(true).write(true).create(true).truncate(true).open(output)?; f.set_len(total)?; let mut map=unsafe{MmapMut::map_mut(&f)?};
        map[..8].copy_from_slice(MAGIC);map[8..16].copy_from_slice(&unique.to_le_bytes());map[16..24].copy_from_slice(&records.to_le_bytes());map[24..32].copy_from_slice(&(HEADER as u64).to_le_bytes());let body=HEADER+unique as usize*DIR;map[32..40].copy_from_slice(&(body as u64).to_le_bytes());
        let mut reader=BufReader::new(File::open(sorted)?); let mut dir_i=0usize; let mut row_i=0usize; let mut current:Option<u64>=None; let mut start=0usize;
        while let Some((key,row))=read_sparse(&mut reader)? {
            if current!=Some(key) {
                if let Some(old)=current { write_dir(&mut map,dir_i,old,start,row_i-start);dir_i+=1; }
                current=Some(key);start=row_i;
            }
            let off=body+row_i*4;map[off..off+4].copy_from_slice(&row.to_le_bytes());row_i+=1;
        }
        if let Some(old)=current { write_dir(&mut map,dir_i,old,start,row_i-start);dir_i+=1; }
        if dir_i!=unique as usize||row_i!=records as usize{return Err(io::Error::new(io::ErrorKind::InvalidData,"postings conversion count mismatch"));}
        map.flush()
    }

    pub fn open(path:impl AsRef<Path>)->io::Result<Self>{
        let f=File::open(path)?;let map=unsafe{Mmap::map(&f)?};if map.len()<HEADER||&map[..8]!=MAGIC{return Err(io::Error::new(io::ErrorKind::InvalidData,"invalid postings header"));}
        let keys=u64::from_le_bytes(map[8..16].try_into().unwrap()) as usize;let rows=u64::from_le_bytes(map[16..24].try_into().unwrap()) as usize;let dir=u64::from_le_bytes(map[24..32].try_into().unwrap()) as usize;let body=u64::from_le_bytes(map[32..40].try_into().unwrap()) as usize;
        let expected=body.checked_add(rows.checked_mul(4).ok_or_else(||io::Error::new(io::ErrorKind::InvalidData,"postings overflow"))?).ok_or_else(||io::Error::new(io::ErrorKind::InvalidData,"postings overflow"))?;
        if dir!=HEADER||body!=HEADER+keys*DIR||expected!=map.len(){return Err(io::Error::new(io::ErrorKind::InvalidData,"postings layout mismatch"));}
        Ok(Self{map,keys,rows,body})
    }
    fn dir(&self,i:usize)->(u64,usize,usize){let o=HEADER+i*DIR;(u64::from_le_bytes(self.map[o..o+8].try_into().unwrap()),u64::from_le_bytes(self.map[o+8..o+16].try_into().unwrap()) as usize,u32::from_le_bytes(self.map[o+16..o+20].try_into().unwrap()) as usize)}
    fn bounds(&self,key:u64)->Option<(usize,usize)>{let(mut l,mut r)=(0,self.keys);while l<r{let m=l+(r-l)/2;if self.dir(m).0<key{l=m+1}else{r=m}}if l>=self.keys{return None}let(k,s,n)=self.dir(l);if k==key{Some((s,n))}else{None}}
    fn row_at(&self,i:usize)->u32{let o=self.body+i*4;u32::from_le_bytes(self.map[o..o+4].try_into().unwrap())}
    fn lower_bound_row(&self,start:usize,end:usize,target:u32)->usize{let(mut l,mut r)=(start,end);while l<r{let m=l+(r-l)/2;if self.row_at(m)<target{l=m+1}else{r=m}}l}
    pub fn row_count(&self,key:u64)->usize{self.bounds(key).map(|x|x.1).unwrap_or(0)}
    pub fn rows(&self,key:u64)->Vec<u32>{let Some((s,n))=self.bounds(key)else{return Vec::new()};(s..s+n).map(|i|self.row_at(i)).collect()}
    pub fn rows_from(&self,key:u64,first_row:u32,limit:usize)->Vec<u32>{if limit==0{return Vec::new()}let Some((s,n))=self.bounds(key)else{return Vec::new()};let end=s+n;let start=self.lower_bound_row(s,end,first_row);(start..end).take(limit).map(|i|self.row_at(i)).collect()}
    pub fn contains_row(&self,key:u64,row:u32)->bool{let Some((s,n))=self.bounds(key)else{return false};let end=s+n;let pos=self.lower_bound_row(s,end,row);pos<end&&self.row_at(pos)==row}
    pub fn intersect_rows(&self,key:u64,seed:&[u32])->Vec<u32>{
        if seed.is_empty(){return Vec::new()}
        let Some((s,n))=self.bounds(key)else{return Vec::new()};
        let end=s+n;let mut out=Vec::with_capacity(seed.len().min(n));
        if n>seed.len().saturating_mul(16){
            for &row in seed{let pos=self.lower_bound_row(s,end,row);if pos<end&&self.row_at(pos)==row{out.push(row)}}
            return out;
        }
        let(mut i,mut j)=(0usize,self.lower_bound_row(s,end,seed[0]));
        while i<seed.len()&&j<end{let r=self.row_at(j);match seed[i].cmp(&r){std::cmp::Ordering::Less=>i+=1,std::cmp::Ordering::Greater=>j+=1,std::cmp::Ordering::Equal=>{out.push(r);i+=1;j+=1}}}
        out
    }
    pub fn total_rows(&self)->usize{self.rows}
}
fn write_dir(map:&mut [u8],i:usize,key:u64,start:usize,len:usize){let o=HEADER+i*DIR;map[o..o+8].copy_from_slice(&key.to_le_bytes());map[o+8..o+16].copy_from_slice(&(start as u64).to_le_bytes());map[o+16..o+20].copy_from_slice(&(len as u32).to_le_bytes());map[o+20..o+24].fill(0);}

#[cfg(test)]mod tests{use super::*;use std::io::Write;#[test]fn builds_and_intersects(){let d=tempfile::tempdir().unwrap();let s=d.path().join("s");let p=d.path().join("p");let mut f=File::create(&s).unwrap();for(k,r)in[(1u64,0u32),(1,3),(1,9),(4,1),(4,8)]{f.write_all(&k.to_le_bytes()).unwrap();f.write_all(&r.to_le_bytes()).unwrap()}drop(f);PostingHierarchy::build_from_sorted(&s,&p,5).unwrap();let x=PostingHierarchy::open(p).unwrap();assert_eq!(x.row_count(1),3);assert_eq!(x.rows(4),vec![1,8]);assert_eq!(x.rows_from(1,3,2),vec![3,9]);assert!(x.contains_row(1,9));assert!(!x.contains_row(1,8));assert_eq!(x.intersect_rows(1,&[0,2,3,8]),vec![0,3]);}}
