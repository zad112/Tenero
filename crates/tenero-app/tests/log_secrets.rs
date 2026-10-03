//! G6 of the threat model: no secret reaches a log line. Two checks work together: this file reads every call site that writes a log line
//! (or prints) in the source of the programs and the libraries under them and refuses one whose arguments name a secret; the end-to-end
//! check (`daemon.rs`, `no_secret_reaches_the_log_at_the_most_verbose_level`) runs real nodes at the most verbose level and looks for the
//! secrets' actual values in the logs they wrote. The first catches a call site that is never run (an error path); the second catches a
//! secret that gets in under a name this file does not know.

use std::path::{Path, PathBuf};

/// Words that name a secret. (`seed` alone is not here: a seed is also a node's address in the network code. A wallet's seed is
/// `wallet_seed`, `mnemonic` or `master` in the code, and the end-to-end check looks for the seed's value anyway.)
const SECRET_WORDS: &[&str] = &[
    "mnemonic",
    "passphrase",
    "password",
    "secret",
    "spend_key",
    "view_key",
    "private",
    "cookie",
    "node_key",
    "signing_key",
    "master",
    "wallet_seed",
    "seed_hex",
];

/// Names that contain a secret word and are not secrets: the NAME of the file that holds the cookie, not its content.
const BENIGN: &[&str] = &["cookie_file"];

/// What starts a statement that writes a line.
const CALLS: &[&str] = &[
    ".info(",
    ".warn(",
    ".error(",
    ".debug(",
    "log_event(",
    "self.log(",
    "(cfg.log)(",
    "(self.cfg.log)(",
    "eprintln!(",
    "println!(",
    "eprint!(",
    "print!(",
];

/// The arguments of the call that starts at `open` (the index just after its `(`), up to the matching `)`; string literals are skipped
/// whole, so a `)` inside text does not end it. `None` if the source ends first.
fn args_from(src: &str, open: usize) -> Option<String> {
    let b = src.as_bytes();
    let (mut depth, mut i) = (1usize, open);
    while i < b.len() {
        match b[i] {
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(src[open..i].to_string());
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// What names a value in a call's arguments: the code outside string literals, and the `{names}` interpolated inside them. (The words of the
/// text itself, "the cookie is missing", are not values.)
fn named_values(args: &str) -> String {
    let (mut out, mut in_str) = (String::new(), false);
    let mut chars = args.chars().peekable();
    while let Some(c) = chars.next() {
        if in_str {
            match c {
                '\\' => {
                    chars.next();
                }
                '"' => {
                    in_str = false;
                    out.push(' ');
                }
                '{' if chars.peek() == Some(&'{') => {
                    chars.next(); // `{{` is a brace in the text
                }
                '}' if chars.peek() == Some(&'}') => {
                    chars.next();
                }
                '{' => {
                    out.push(' ');
                    for d in chars.by_ref() {
                        if d == '}' || d == ':' {
                            break;
                        }
                        out.push(d);
                    }
                    out.push(' ');
                }
                _ => {}
            }
        } else if c == '"' {
            in_str = true;
        } else {
            out.push(c);
        }
    }
    out
}

/// Strips `//` comments (outside strings) so that a comment cannot start or hide a call.
fn without_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    for line in src.lines() {
        let (mut in_str, mut cut) = (false, line.len());
        let b = line.as_bytes();
        let mut i = 0;
        while i < b.len() {
            match b[i] {
                b'\\' if in_str => i += 1,
                b'"' => in_str = !in_str,
                b'/' if !in_str && i + 1 < b.len() && b[i + 1] == b'/' => {
                    cut = i;
                    break;
                }
                _ => {}
            }
            i += 1;
        }
        out.push_str(&line[..cut]);
        out.push('\n');
    }
    out
}

/// Every call that writes a line, with the secret words its values name: `(call text, words)`.
fn scan(src: &str) -> (usize, Vec<(String, Vec<&'static str>)>) {
    let src = without_comments(src);
    let (mut calls, mut bad) = (0, vec![]);
    for call in CALLS {
        let mut from = 0;
        while let Some(at) = src[from..].find(call) {
            let open = from + at + call.len();
            // `println!(` is not also an `eprintln!(`, nor `print!(` a `println!(`'s ending: a macro or function name starts a word
            let starts_word = call.starts_with(|c: char| c.is_alphabetic() || c == '(');
            let before = src[..from + at].chars().next_back();
            from = open;
            if starts_word && before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                continue;
            }
            let Some(args) = args_from(&src, open) else {
                continue;
            };
            calls += 1;
            let mut values = named_values(&args).to_lowercase();
            for ok in BENIGN {
                values = values.replace(ok, " ");
            }
            let words: Vec<&'static str> = SECRET_WORDS
                .iter()
                .copied()
                .filter(|w| values.contains(w))
                .collect();
            if !words.is_empty() {
                bad.push((format!("{call}{args})"), words));
            }
        }
    }
    (calls, bad)
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

#[test]
fn no_call_site_in_the_programs_or_the_libraries_under_them_logs_a_secret_by_name() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let (mut files, mut calls, mut violations) = (0, 0, vec![]);
    for name in [
        "tenero-app",
        "tenero-net",
        "tenero-node",
        "tenero-miner",
        "tenero-wallet",
        "tenero-chain",
        "tenero-store",
        "tenero-core",
        "tenero-crypto",
        "tenero-gpu",
    ] {
        let mut list = vec![];
        rust_files(&crates.join(name).join("src"), &mut list);
        for f in list {
            files += 1;
            let (n, bad) = scan(&std::fs::read_to_string(&f).unwrap());
            calls += n;
            for (call, words) in bad {
                violations.push(format!("{}: {call}  (names {words:?})", f.display()));
            }
        }
    }
    assert!(files > 40, "only {files} source files were looked at");
    assert!(calls > 30, "only {calls} call sites were looked at");
    assert!(
        violations.is_empty(),
        "a log line names a secret:\n{}",
        violations.join("\n")
    );
}

// ---- the scanner itself: it must be able to fail -----------------------------------------------------------------------------------

#[test]
fn the_scanner_finds_a_secret_in_an_argument_in_a_placeholder_and_across_lines() {
    let bad = |src: &str| scan(src).1.len();
    assert_eq!(bad(r#"log.info(&format!("cookie is {cookie}"));"#), 1);
    assert_eq!(bad(r#"log.debug(&format!("key {}", node_key));"#), 1);
    assert_eq!(
        bad("log.warn(&format!(\n    \"a {}\",\n    wallet.passphrase\n));"),
        1
    );
    assert_eq!(bad(r#"eprintln!("{}", spend_key)"#), 1);
    assert_eq!(bad(r#"self.log(&format!("{mnemonic:?}"))"#), 1);
    // an inner call's parenthesis inside a string does not end it early
    assert_eq!(bad(r#"log.info(&format!("(a) {}", master_key))"#), 1);
}

#[test]
fn the_scanner_leaves_alone_words_in_the_text_and_things_that_are_not_calls() {
    let bad = |src: &str| scan(src).1.len();
    // the words of the message are not values
    assert_eq!(
        bad(r#"log.error("the cookie is missing: start the node first")"#),
        0
    );
    assert_eq!(bad(r#"log.info("a private data directory")"#), 0);
    assert_eq!(
        bad(r#"log.info(&format!("{{cookie}} {}", height))"#),
        0,
        "an escaped brace is text"
    );
    // a comment is not a call
    assert_eq!(bad("// log.info(&format!(\"{}\", cookie));"), 0);
}

#[test]
fn the_scanner_counts_what_it_looked_at() {
    let (n, _) = scan("log.info(\"a\"); log.warn(\"b\"); eprintln!(\"c\"); other(\"d\");");
    assert_eq!(n, 3);
    assert_eq!(scan("").0, 0);
}
