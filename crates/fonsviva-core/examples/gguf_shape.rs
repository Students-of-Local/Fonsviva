//! Maintainer tool: prints the shape of GGUF files (or header prefixes) as JSON,
//! for the `shape` field of catalog/models.json.
//! `cargo run -p fonsviva-core --example gguf_shape -- <file>... [--keys]`

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let keys = args.iter().any(|a| a == "--keys");
    for path in args.iter().filter(|a| !a.starts_with("--")) {
        let h = match fonsviva_core::gguf::read_header_file(path.as_ref()) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("{path}: {e:#}");
                continue;
            }
        };
        if keys {
            for (k, v) in &h.kv {
                if !k.starts_with("tokenizer.") {
                    println!("  {k} = {v:?}");
                }
            }
        }
        match fonsviva_core::gguf::Shape::from_header(&h) {
            Ok(s) => println!("{path}\n{}", serde_json::to_string(&s).unwrap()),
            Err(e) => eprintln!("{path}: {e:#}"),
        }
    }
}
