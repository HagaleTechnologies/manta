//! Regenerate the independent callsign-pattern reference list from MASTER.SCP.

use std::io::Write;
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let source = match args.next() {
        Some(path) => std::fs::read_to_string(path)?,
        None => manta_spot::MASTER_SCP.to_owned(),
    };
    if args.next().is_some() {
        return Err("usage: gen_pattern_list [MASTER.SCP]".into());
    }
    let generated = manta_spot::pattern::derive_list(&source);
    let table = manta_spot::pattern::Table::parse(&generated)?;
    let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("data/patterns.lst");
    let temporary = target.with_file_name(format!(".patterns.lst.{}.tmp", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let written = (|| -> std::io::Result<()> {
        file.write_all(generated.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, &target)
    })();
    if let Err(error) = written {
        let _ = std::fs::remove_file(&temporary);
        return Err(error.into());
    }
    println!(
        "wrote {} ({} patterns, {} common)",
        target.display(),
        table.len(),
        table.common_len()
    );
    Ok(())
}
