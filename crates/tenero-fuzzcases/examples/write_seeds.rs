//! Writes the seed inputs of every fuzz target to `<dir>/<target>/<name>` (the corpus libFuzzer starts from):
//! `cargo run -p tenero-fuzzcases --example write_seeds -- fuzz/corpus`.

fn main() {
    let dir = std::env::args()
        .nth(1)
        .expect("usage: write_seeds <corpus directory>");
    let mut n = 0;
    for (target, name, data) in tenero_fuzzcases::seeds() {
        let sub = std::path::Path::new(&dir).join(target);
        std::fs::create_dir_all(&sub).unwrap();
        let file: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        std::fs::write(sub.join(file), data).unwrap();
        n += 1;
    }
    println!("{n} seeds written to {dir}");
}
