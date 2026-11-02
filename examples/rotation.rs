use bitcask_lite::{Bitcask, Options};
use std::io;

fn main() -> io::Result<()> {
    let options = Options {
        max_segment_bytes: 256,
        compact_after_segments: 3,
        ..Options::default()
    };
    let mut db = Bitcask::open_with_options("rotation.db", options)?;
    for n in 0..20_u8 {
        db.put(b"counter", [n; 80])?;
    }
    println!("latest: {:?}", db.get(b"counter")?.map(|value| value[0]));
    println!("storage: {:?}", db.stats());
    Ok(())
}
