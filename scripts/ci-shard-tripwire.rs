// MAN-565: built once on Windows, copied under each guarded compiler's native name.
fn main() {
    let executable = std::env::current_exe().expect("tripwire executable path");
    let tool = executable
        .file_stem()
        .expect("tripwire filename")
        .to_string_lossy();
    eprintln!("MAN-518 tripwire: a shard invoked {tool}");
    std::process::exit(97);
}
