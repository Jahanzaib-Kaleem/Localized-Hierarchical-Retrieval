use lhr::{Engine,Predicate,Segment};use std::{fs,io::Write};
#[test]fn routed_query_is_exact(){let d=tempfile::tempdir().unwrap();fs::create_dir(d.path().join("canonical")).unwrap();fs::create_dir(d.path().join("routing")).unwrap();
 let rows=[1u8,2, 1,3, 2,2, 1,2, 3,1, 1,2];Segment::write(d.path().join("canonical/s0.lhr"),6,2,1,&rows).unwrap();
 let mut f=fs::File::create(d.path().join("routing/h0.bin")).unwrap();for(k,p)in[(6u64,0u32),(7,0),(10,0),(13,1),(6,1)]{f.write_all(&k.to_le_bytes()).unwrap();f.write_all(&p.to_le_bytes()).unwrap();}drop(f);
 // sorted order is mandatory; rewrite correctly sorted records
 let mut f=fs::File::create(d.path().join("routing/h0.bin")).unwrap();for(k,p)in[(6u64,0u32),(6,1),(7,0),(10,0),(13,1)]{f.write_all(&k.to_le_bytes()).unwrap();f.write_all(&p.to_le_bytes()).unwrap();}
 fs::write(d.path().join("manifest.json"),r#"{"page_rows":3,"pages":2,"cardinalities":[4,4],"segments":[{"file":"s0.lhr","first_page":0}],"hierarchies":[{"file":"h0.bin","columns":[0,1]}]}"#).unwrap();
 let e=Engine::open(d.path()).unwrap();let(h,c)=e.query_count(&[Predicate{column:0,value:1},Predicate{column:1,value:2}]);assert_eq!(h,3);assert_eq!(c,6);
}
