//! Loading the golden vectors in `tests/vectors/` (see its README).

use serde_json::Value;
use std::path::PathBuf;

/// The directory holding the golden vectors, found from this crate's location.
pub fn vectors_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/vectors")
}

/// Loads `<name>.json`, checking that its `schema` is 1 and its `name` field matches.
pub fn load(name: &str) -> Result<Value, String> {
    let path = vectors_dir().join(format!("{name}.json"));
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let value: Value = serde_json::from_str(&text).map_err(|e| format!("{name}: bad JSON: {e}"))?;
    if value["schema"] != 1 {
        return Err(format!("{name}: unsupported schema {}", value["schema"]));
    }
    if value["name"] != name {
        return Err(format!("{name}: name field is {}", value["name"]));
    }
    Ok(value)
}

/// Decodes a hex string (either case) into bytes.
pub fn hex(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err(format!("odd-length hex string ({} characters)", s.len()));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            s.get(i..i + 2)
                .filter(|p| p.bytes().all(|b| b.is_ascii_hexdigit()))
                .and_then(|p| u8::from_str_radix(p, 16).ok())
                .ok_or_else(|| format!("bad hex at position {i}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [&str; 12] = [
        "chacha20",
        "matmulhash_small",
        "matmulhash_real",
        "matmulhash_deep",
        "matmulhash_full",
        "pow_misc",
        "emission",
        "difficulty",
        "fees_and_size",
        "units",
        "chains",
        "legacy_account_model",
    ];

    #[test]
    fn every_vector_file_loads_with_its_name() {
        for name in ALL {
            load(name).unwrap_or_else(|e| panic!("{e}"));
        }
    }

    #[test]
    fn the_full_vector_has_256_slice_hashes() {
        let full = load("matmulhash_full").unwrap();
        let hashes = full["slice_sha256"].as_array().unwrap();
        assert_eq!(hashes.len(), 256);
        for h in hashes {
            assert_eq!(hex(h.as_str().unwrap()).unwrap().len(), 32);
        }
    }

    #[test]
    fn hex_decoding() {
        assert_eq!(hex("00ffAb").unwrap(), vec![0, 255, 171]);
        assert!(hex("abc").is_err());
        assert!(hex("zz").is_err());
        assert!(hex("+1").is_err());
    }
}
