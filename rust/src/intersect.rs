pub fn intersect_sorted(a:&[u32],b:&[u32])->Vec<u32>{
 let mut out=Vec::with_capacity(a.len().min(b.len())); let(mut i,mut j)=(0,0);
 while i<a.len()&&j<b.len(){
  match a[i].cmp(&b[j]){
   std::cmp::Ordering::Less=>i+=1,
   std::cmp::Ordering::Greater=>j+=1,
   std::cmp::Ordering::Equal=>{out.push(a[i]);i+=1;j+=1;}
  }
 }
 out
}
#[cfg(test)] mod tests{use super::*;#[test]fn intersection(){assert_eq!(intersect_sorted(&[1,3,7,9],&[2,3,7,8]),vec![3,7]);}}
