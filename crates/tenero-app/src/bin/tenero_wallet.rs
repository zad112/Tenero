//! The wallet program. See `tenero-wallet help`. Interim output scheme, unaudited, no value.

use tenero_app::wallet_cli::{run, Io};
use zeroize::Zeroizing;

struct Terminal;

impl Io for Terminal {
    fn passphrase(&mut self, prompt: &str) -> Result<Zeroizing<String>, String> {
        rpassword::prompt_password(prompt)
            .map(Zeroizing::new)
            .map_err(|e| format!("cannot read from the terminal: {e}"))
    }

    fn say(&mut self, line: &str) {
        println!("{line}");
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = run(&args, &mut Terminal) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
