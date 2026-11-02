use bitcask_lite::Bitcask;
use std::io;

fn main() -> io::Result<()> {
    let mut db = Bitcask::open("example.db")?;
    db.put(b"language", b"Rust")?;
    if let Some(value) = db.get(b"language")? {
        println!("{}", String::from_utf8_lossy(&value));
    }
    db.delete(b"language")?;
    db.compact()?;
    Ok(())
}
