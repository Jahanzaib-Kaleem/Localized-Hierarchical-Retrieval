use std::{cmp::Reverse, collections::BinaryHeap, fs::{self, File}, io::{self, BufReader, BufWriter, Read, Write}, path::{Path, PathBuf}};

const RECORD_BYTES: usize = 12;

type Record = (u64, u32);

fn read_record<R: Read>(r: &mut R) -> io::Result<Option<Record>> {
    let mut buf = [0u8; RECORD_BYTES];
    let mut got = 0usize;
    while got < RECORD_BYTES {
        match r.read(&mut buf[got..])? {
            0 if got == 0 => return Ok(None),
            0 => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated hierarchy record")),
            n => got += n,
        }
    }
    Ok(Some((u64::from_le_bytes(buf[..8].try_into().unwrap()), u32::from_le_bytes(buf[8..].try_into().unwrap()))))
}

fn write_record<W: Write>(w: &mut W, (key, page): Record) -> io::Result<()> {
    w.write_all(&key.to_le_bytes())?;
    w.write_all(&page.to_le_bytes())
}

pub fn external_sort(input: impl AsRef<Path>, output: impl AsRef<Path>, max_records: usize) -> io::Result<u64> {
    if max_records == 0 { return Err(io::Error::new(io::ErrorKind::InvalidInput, "max_records must be > 0")); }
    let input = input.as_ref();
    let output = output.as_ref();
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;

    let mut reader = BufReader::new(File::open(input)?);
    let mut runs: Vec<PathBuf> = Vec::new();
    let mut chunk = Vec::with_capacity(max_records);
    let mut run_no = 0usize;

    loop {
        chunk.clear();
        while chunk.len() < max_records {
            match read_record(&mut reader)? { Some(r) => chunk.push(r), None => break }
        }
        if chunk.is_empty() { break; }
        chunk.sort_unstable();
        chunk.dedup();
        let path = parent.join(format!(".lhr-run-{run_no:06}.bin"));
        run_no += 1;
        let mut w = BufWriter::new(File::create(&path)?);
        for &r in &chunk { write_record(&mut w, r)?; }
        w.flush()?;
        runs.push(path);
    }

    if runs.is_empty() { File::create(output)?; return Ok(0); }

    let mut readers: Vec<BufReader<File>> = runs.iter().map(File::open).collect::<io::Result<Vec<_>>>()?.into_iter().map(BufReader::new).collect();
    let mut heap: BinaryHeap<Reverse<(u64, u32, usize)>> = BinaryHeap::new();
    for (i, r) in readers.iter_mut().enumerate() {
        if let Some((k, p)) = read_record(r)? { heap.push(Reverse((k, p, i))); }
    }

    let mut out = BufWriter::new(File::create(output)?);
    let mut last: Option<Record> = None;
    let mut count = 0u64;
    while let Some(Reverse((k, p, i))) = heap.pop() {
        let record = (k, p);
        if last != Some(record) { write_record(&mut out, record)?; last = Some(record); count += 1; }
        if let Some((nk, np)) = read_record(&mut readers[i])? { heap.push(Reverse((nk, np, i))); }
    }
    out.flush()?;
    for run in runs { let _ = fs::remove_file(run); }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sorts_and_deduplicates_across_runs() {
        let d = tempfile::tempdir().unwrap();
        let input = d.path().join("in.bin");
        let output = d.path().join("out.bin");
        let mut w = BufWriter::new(File::create(&input).unwrap());
        for r in [(9,3),(1,7),(9,3),(2,1),(1,2),(1,7),(8,0)] { write_record(&mut w, r).unwrap(); }
        w.flush().unwrap();
        assert_eq!(external_sort(&input, &output, 2).unwrap(), 5);
        let mut r = BufReader::new(File::open(output).unwrap());
        let mut got = Vec::new(); while let Some(x) = read_record(&mut r).unwrap() { got.push(x); }
        assert_eq!(got, vec![(1,2),(1,7),(2,1),(8,0),(9,3)]);
    }
}
