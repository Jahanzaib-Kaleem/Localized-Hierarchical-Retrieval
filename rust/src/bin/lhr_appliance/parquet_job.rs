//! One bounded admin MCP entry point to import staged Parquet shards into a named
//! LHR bucket. Source files must already live in /data/control/sandbox/apollo-import-source.
//! This process runs the appliance's native importer as an OS child and limits output
//! size and free disk space, avoiding a long-lived MCP HTTP request.

use super::McpState;
use fs2::FileExt;
use lhr::{create_bucket, dataset_status};
use rand::{rngs::OsRng, RngCore};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    ffi::CString,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const MAX_GIB: u64 = 70;
const MIN_FREE_GIB: u64 = 10;
const GIB: u64 = 1024 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    action: String,
    #[serde(default)]
    bucket_id: Option<String>,
    #[serde(default)]
    bucket_name: Option<String>,
    #[serde(default)]
    max_bucket_gib: Option<u64>,
    #[serde(default)]
    job_id: Option<String>,
}

pub(super) fn tool_schema() -> Value {
    json!({
        "type":"object","required":["action"],
        "properties":{
            "action":{"type":"string","enum":["start","status","info"]},
            "bucket_id":{"type":"string","description":"New named LHR bucket ID, such as apollo-full, required for start"},
            "bucket_name":{"type":"string","description":"Display name for the new bucket, required for start"},
            "max_bucket_gib":{"type":"integer","minimum":1,"maximum":70,"default":70,
                "description":"Hard cap on generated LHR bucket storage, not the original staged Parquet files. Aborts and cleans the new bucket if exceeded."},
            "job_id":{"type":"string","description":"Opaque import job ID returned by start, required for status"}
        },
        "additionalProperties":false
    })
}

fn now_epoch() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}
fn random_id() -> String {
    let mut bytes=[0u8;16];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b|format!("{b:02x}")).collect::<String>()
}
fn valid_id(id:&str)->bool {
    id.len()==32 && id.bytes().all(|b|b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn job_dir(root:&Path)->PathBuf {root.join("control").join("parquet-import-jobs")}
fn source_dir(root:&Path)->PathBuf {root.join("control").join("sandbox").join("apollo-import-source")}
fn job_status_file(root:&Path,id:&str)->PathBuf {job_dir(root).join(format!("{id}.json"))}
fn save_status(root:&Path,id:&str,value:&Value)->Result<(),String>{
    let dir=job_dir(root);
    fs::create_dir_all(&dir).map_err(|e|e.to_string())?;
    let temp=dir.join(format!(".{id}.{}.tmp",std::process::id()));
    let bytes=serde_json::to_vec_pretty(value).map_err(|e|e.to_string())?;
    let mut file=OpenOptions::new().write(true).create_new(true).open(&temp).map_err(|e|e.to_string())?;
    file.write_all(&bytes).map_err(|e|e.to_string())?;
    file.sync_all().map_err(|e|e.to_string())?;
    fs::rename(temp,job_status_file(root,id)).map_err(|e|e.to_string())
}
fn directory_size(path:&Path)->Result<u64,String>{
    let mut total=0u64;
    let mut pending=vec![path.to_path_buf()];
    while let Some(item)=pending.pop(){
        for entry in fs::read_dir(&item).map_err(|e|e.to_string())?{
            let entry=entry.map_err(|e|e.to_string())?;
            let meta=entry.path().symlink_metadata().map_err(|e|e.to_string())?;
            if meta.file_type().is_dir(){pending.push(entry.path());}
            else if meta.file_type().is_file(){total=total.saturating_add(meta.len());}
        }
    }
    Ok(total)
}
fn free_bytes(path:&Path)->Result<u64,String>{
    let c=CString::new(path.as_os_str().as_bytes()).map_err(|e|e.to_string())?;
    let mut result=std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe{libc::statvfs(c.as_ptr(),result.as_mut_ptr())}!=0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let stats=unsafe{result.assume_init()};
    Ok(stats.f_bavail.saturating_mul(stats.f_frsize))
}
fn tail(path:&Path)->String {
    let Ok(mut file)=File::open(path) else {return String::new();};
    let mut bytes=Vec::new();
    if file.read_to_end(&mut bytes).is_err(){return String::new();}
    String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(4096)..]).into_owned()
}
fn source_files(dir:&Path)->Result<Vec<PathBuf>,String>{
    if !dir.is_dir(){return Err(format!("source directory absent: {}",dir.display()));}
    let mut files=Vec::new();
    for e in fs::read_dir(dir).map_err(|e|e.to_string())?{
        let e=e.map_err(|e|e.to_string())?;
        let name=e.file_name();
        let name=name.to_string_lossy();
        if name.starts_with("apollo_data_") && name.ends_with(".parquet") {
            let meta=e.path().symlink_metadata().map_err(|e|e.to_string())?;
            if !meta.file_type().is_file(){return Err("Parquet source is not a regular file".into());}
            files.push(e.path());
        }
    }
    files.sort();
    if files.is_empty(){return Err("no Apollo Parquet files staged".into());}
    if files.len()>1024{return Err("too many source shards".into());}
    for name in ["apollo-schema.json","apollo-profile.json"] {
        if !dir.join(name).is_file(){return Err(format!("missing {name} in staging directory"));}
    }
    Ok(files)
}

pub(super) fn call(state:&McpState,arguments:Value)->Result<Value,String>{
    let args:Args=serde_json::from_value(arguments).map_err(|e|e.to_string())?;
    match args.action.as_str() {
        "info" => Ok(json!({
            "source_dir":"/data/control/sandbox/apollo-import-source",
            "max_bucket_gib":MAX_GIB,
            "min_free_gib":MIN_FREE_GIB,
            "importer":"lhr-parquet-import",
            "requirements":"Stage all source .parquet files, apollo-schema.json and apollo-profile.json before starting"
        })),
        "status" => {
            let id=args.job_id.as_deref().ok_or("job_id required")?;
            if !valid_id(id){return Err("invalid job_id".into());}
            let bytes=fs::read(job_status_file(&state.root,id)).map_err(|e|e.to_string())?;
            serde_json::from_slice(&bytes).map_err(|e|e.to_string())
        }
        "start" => start_job(state,args),
        _ => Err("action must be start, status or info".into()),
    }
}

fn start_job(state:&McpState,args:Args)->Result<Value,String>{
    let id=args.bucket_id.ok_or("bucket_id required")?;
    let name=args.bucket_name.ok_or("bucket_name required")?;
    if id=="default" || name.trim().is_empty(){return Err("a new named bucket is required".into());}
    let max_gib=args.max_bucket_gib.unwrap_or(MAX_GIB);
    if !(1..=MAX_GIB).contains(&max_gib){return Err("max_bucket_gib must be 1..=70".into());}
    let source=source_dir(&state.root);
    let files=source_files(&source)?;
    let schema=source.join("apollo-schema.json");
    let profile=source.join("apollo-profile.json");
    // Verify the provided schema before creating anything in the bucket catalog.
    let parsed=lhr::read_schema_file(&schema).map_err(|e|e.to_string())?;
    if parsed.columns.is_empty(){return Err("source schema has no columns".into());}
    if free_bytes(&state.root)? < (max_gib+MIN_FREE_GIB)*GIB {
        return Err(format!("need {} GiB free before import to preserve the {} GiB reserve",max_gib+MIN_FREE_GIB,MIN_FREE_GIB));
    }
    let control=job_dir(&state.root);
    fs::create_dir_all(&control).map_err(|e|e.to_string())?;
    let lock=OpenOptions::new().create(true).read(true).write(true)
        .open(control.join(".active.lock")).map_err(|e|e.to_string())?;
    lock.try_lock_exclusive().map_err(|_|"another Parquet import is active".to_string())?;

    let bucket=create_bucket(&state.root,&id,&name).map_err(|e|e.to_string())?;
    let root=state.root.clone();
    let bucket_root=root.join("buckets").join(&id);
    let job=random_id();
    let initial=json!({
        "job_id":job,"status":"starting","bucket_id":id,"bucket_name":name,
        "source_files":files.len(),"source_schema_columns":parsed.columns.len(),
        "max_bucket_gib":max_gib,"started_at_epoch":now_epoch()
    });
    save_status(&root,&job,&initial)?;
    let job_clone=job.clone();
    thread::Builder::new().name("lhr-parquet-import-monitor".into()).spawn(move ||{
        let _lock=lock;
        let outcome=run_import(&root,&job_clone,&bucket_root,&schema,&profile,&files,max_gib);
        if let Err(error)=outcome{
            let _=save_status(&root,&job_clone,&json!({
                "job_id":job_clone,"status":"failed","error":error,"finished_at_epoch":now_epoch()
            }));
        }
    }).map_err(|e|e.to_string())?;
    Ok(json!({
        "job_id":job,"status":"queued","bucket_id":id,"bucket_name":name,
        "source_files":files.len(),"max_bucket_gib":max_gib,
        "note":"Native Parquet import runs with a hard bucket-size cap and free-disk reserve. Monitor with action=status."
    }))
}

fn run_import(root:&Path,job:&str,bucket:&Path,schema:&Path,profile:&Path,sources:&[PathBuf],max_gib:u64)->Result<(),String>{
    let stdout_path=job_dir(root).join(format!("{job}.stdout"));
    let stderr_path=job_dir(root).join(format!("{job}.stderr"));
    let out=File::create(&stdout_path).map_err(|e|e.to_string())?;
    let err=File::create(&stderr_path).map_err(|e|e.to_string())?;
    let mut command=Command::new("/usr/local/bin/lhr-parquet-import");
    command
        .arg("--root").arg(bucket)
        .arg("--schema").arg(schema)
        .arg("--profile").arg(profile)
        .arg("--max-sort-records").arg("250000")
        .arg("--dictionary-run-bytes").arg("16777216")
        .args(sources)
        .stdin(Stdio::null()).stdout(Stdio::from(out)).stderr(Stdio::from(err));
    let mut child=command.spawn().map_err(|e|e.to_string())?;
    let mut prior_size=0u64;
    let max_bytes=max_gib*GIB;
    loop {
        let finished=child.try_wait().map_err(|e|e.to_string())?;
        let size=directory_size(bucket).unwrap_or(prior_size);
        prior_size=size;
        let remaining=free_bytes(root).unwrap_or(0);
        let hard_cap=size>max_bytes || remaining < MIN_FREE_GIB*GIB;
        if hard_cap {
            let _=child.kill();
            let _=child.wait();
            // This was an exclusively new bucket created for this job; discard
            // its incomplete generations rather than retaining an oversized build.
            let _=fs::remove_dir_all(bucket.join("generations"));
            let _=save_status(root,job,&json!({
                "job_id":job,"status":"aborted_storage_cap","bucket_bytes_at_abort":size,
                "max_bucket_gib":max_gib,"free_bytes_at_abort":remaining,
                "finished_at_epoch":now_epoch(),"note":"Source Parquet files are untouched"
            }));
            return Ok(());
        }
        if let Some(status)=finished {
            if !status.success(){
                let error=tail(&stderr_path);
                let _=save_status(root,job,&json!({
                    "job_id":job,"status":"failed","exit_code":status.code(),
                    "bucket_bytes":size,"error":error,"finished_at_epoch":now_epoch()
                }));
                return Ok(());
            }
            let storage=dataset_status(bucket).map_err(|e|e.to_string())?;
            if storage.total_bytes>max_bytes {
                let _=fs::remove_dir_all(bucket.join("generations"));
                let _=save_status(root,job,&json!({
                    "job_id":job,"status":"aborted_storage_cap","bucket_bytes_at_abort":storage.total_bytes,
                    "max_bucket_gib":max_gib,"finished_at_epoch":now_epoch()
                }));
                return Ok(());
            }
            let _=save_status(root,job,&json!({
                "job_id":job,"status":"complete","bucket_bytes":storage.total_bytes,
                "rows":storage.rows,"columns":storage.columns,"finished_at_epoch":now_epoch(),
                "native_import_report":tail(&stdout_path)
            }));
            return Ok(());
        }
        let _=save_status(root,job,&json!({
            "job_id":job,"status":"running","bucket_bytes":size,
            "max_bucket_bytes":max_bytes,"free_bytes":remaining,"checked_at_epoch":now_epoch()
        }));
        thread::sleep(Duration::from_secs(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn validates_job_ids(){assert!(valid_id("1234567890abcdef1234567890abcdef"));assert!(!valid_id("../etc/passwd"));}
    #[test] fn schema_is_bounded(){assert_eq!(tool_schema()["properties"]["max_bucket_gib"]["maximum"],70);}
}
